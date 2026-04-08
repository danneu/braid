# Plan: flock mutex for unlock paths

## Context

Both `braid-unlock.service` and `braid-auto-unlock.service` can race if started concurrently. Each has `ConditionPathIsMountPoint=!/mnt/storage`, but that's a one-time gate — if both pass it before either mounts, they race into `cryptsetup open` on the same devices. The second `cryptsetup open` fails with EBUSY, leaving partial state.

The fix: a shared `flock /run/braid-pool.lock` around pool-mutating commands in the wrapper. This serializes all current and future unlock entry points at the actual critical section, not via systemd unit topology.

## Changes

### 1. `modules/braid/braid-wrapper.sh` — add flock around pool-mutating commands

Restructure the wrapper to:
1. Parse the subcommand **before** running the CLI (move existing arg-parsing loop up).
2. For `unlock|add|recover` (when not `--help`/`--dry-run`): acquire an exclusive flock on fd 9 via `/run/braid-pool.lock`.
3. **For `unlock` only**: after acquiring the lock, re-check `mountpoint -q` — if already mounted, print message and exit 0. This fast-path does **not** apply to `add` or `recover`, which legitimately operate on an already-mounted pool.
4. Run CLI + existing post-processing under the held lock.
5. For all other subcommands: run as before, no lock.

The lock is held via fd 9 for the wrapper's lifetime and released automatically on exit.

```sh
# Acquire pool lock for pool-mutating commands.
# Serializes concurrent unlock paths (e.g. braid-auto-unlock racing
# braid-unlock) without relying on systemd unit ordering.
case "$subcmd" in
  unlock|add|recover)
    if ! $skip_fixup; then
      exec 9>/run/braid-pool.lock
      @flockBin@ 9
    fi
    ;;
esac

# For unlock specifically: re-check after acquiring lock — another
# unlock path may have already mounted the pool while we waited.
# Does NOT apply to add/recover, which operate on a mounted pool.
case "$subcmd" in
  unlock)
    if ! $skip_fixup; then
      if @mountpointBin@ -q "@mountPointPath@" 2>/dev/null; then
        echo "pool already mounted at @mountPointPath@"
        exit 0
      fi
    fi
    ;;
esac

@braidBin@ "$@"
ret=$?
# ... existing post-processing ...
```

### 2. `modules/braid/wrapper.nix` — add `flockBin` substitution

Add `--subst-var-by flockBin '${cfg.packages.utilLinux}/bin/flock'` to the `substitute` call. `util-linux` is already in `toolPackages` so no new dependency.

### 3. `docs/principles.md` — add mutex principle

Add new principle **12. One pool operation at a time**:

> Pool-mutating commands (`unlock`, `add`, `recover`) hold an exclusive `flock` on `/run/braid-pool.lock` for their duration. This serializes concurrent entry points (e.g. `braid-auto-unlock` at boot racing a manual `braid-pool.target` start) at the critical section itself, not via systemd unit topology. After acquiring the lock, `unlock` re-checks `mountpoint -q` and exits cleanly if the pool was already mounted by the winner. `add` and `recover` do not fast-exit — they legitimately operate on mounted pools.

### 4. `docs/decisions/018-systemd-lifecycle.md` — reference the mutex

Add a new section **"Unlock path mutual exclusion"** after "CLI wrapper as synchronization layer", referencing the principle:

> Pool-mutating commands are serialized by `flock /run/braid-pool.lock` in the wrapper. See [Principle 12](../principles.md#12-one-pool-operation-at-a-time).

Add to the "Key design constraints" list:

> **5. One pool operation at a time.** Enforced by `flock` in the wrapper, not unit topology. See Principle 12.

### 5. `tests/module/systemd-lifecycle.py` — add concurrent unlock test

New subtest in the existing test (after the unlock/lock round-trips, before the shutdown test). No new fixture needed — race two wrapper-level `braid unlock --passphrase-stdin` processes:

```python
with subtest("Concurrent unlock attempts serialize via flock"):
    machine.fail("mountpoint -q /mnt/storage")

    # Launch two concurrent unlock attempts through the wrapper.
    # The flock on /run/braid-pool.lock serializes them — the loser
    # re-checks mountpoint after acquiring the lock and exits cleanly.
    machine.succeed(
        f"printf '%s\\n' {pq} | braid unlock --passphrase-stdin >/tmp/unlock-a 2>&1 & "
        f"printf '%s\\n' {pq} | braid unlock --passphrase-stdin >/tmp/unlock-b 2>&1 & "
        f"wait"
    )

    machine.succeed("mountpoint -q /mnt/storage")
    machine.succeed("systemctl is-active braid-online.service")

    out_a = machine.succeed("cat /tmp/unlock-a")
    out_b = machine.succeed("cat /tmp/unlock-b")
    assert "pool already mounted" in out_a or "pool already mounted" in out_b, (
        f"Expected one 'pool already mounted' message.\nA: {out_a}\nB: {out_b}"
    )

    machine.succeed("braid lock")
```

## Files to modify

| File | Change |
|---|---|
| `modules/braid/braid-wrapper.sh` | Restructure: parse subcommand first, add flock + unlock-only re-check |
| `modules/braid/wrapper.nix` | Add `flockBin` substitution variable |
| `docs/principles.md` | Add principle 12: one pool operation at a time |
| `docs/decisions/018-systemd-lifecycle.md` | Add mutex section + design constraint #5, referencing principle 12 |
| `tests/module/systemd-lifecycle.py` | Add concurrent unlock subtest |

## Verification

1. `just test-rust` — no regressions in Rust unit tests.
2. `just test systemd-lifecycle` — existing subtests pass, new race subtest passes.
3. Manual review: read the wrapper to confirm the flock covers the full CLI + post-processing window, that `add`/`recover` are locked but not fast-exited, and that non-mutating commands are unaffected.
